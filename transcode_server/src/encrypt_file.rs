use anyhow::{anyhow, Result};
use chacha20poly1305::{
    aead::{generic_array::GenericArray, Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use std::fs::File;
use std::io::{BufReader, Read, Write};

const CHUNK_SIZE: usize = 262144;
const TAG_SIZE: usize = 16;
const KEY_SIZE: usize = 32;

/// Reads until `buffer` is full or the input ends. A single `read` may return less
/// than is available, and each chunk's size is part of the format: one short chunk
/// makes every chunk after it impossible to decrypt.
fn read_chunk<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn chunk_nonce(chunk_index: u32) -> XNonce {
    let mut bytes = [0u8; 24];
    bytes[..4].copy_from_slice(&chunk_index.to_le_bytes());
    XNonce::clone_from_slice(&bytes)
}

pub fn encrypt_file_xchacha20(
    input_file_path: String,
    output_file_path: String,
    padding: usize,
) -> Result<Vec<u8>> {
    let input = File::open(input_file_path)?;
    let reader = BufReader::new(input);

    let output = File::create(output_file_path)?;

    encrypt_file_xchacha20_internal(reader, output, padding)
}

fn encrypt_file_xchacha20_internal<R: Read>(
    mut reader: R,
    mut output_file: File,
    padding: usize,
) -> Result<Vec<u8>> {
    let key = XChaCha20Poly1305::generate_key(&mut OsRng);
    let cipher = XChaCha20Poly1305::new(&key);

    let mut chunk_index: u32 = 0;

    let mut buffer = vec![0u8; CHUNK_SIZE];

    loop {
        let count = read_chunk(&mut reader, &mut buffer)?;
        if count == 0 {
            break;
        }

        let length = if count < CHUNK_SIZE {
            count + padding
        } else {
            count
        };
        if length > CHUNK_SIZE {
            return Err(anyhow!(
                "padding of {} bytes does not fit the chunk",
                padding
            ));
        }
        // The rest of the buffer still holds the end of the chunk before.
        buffer[count..length].fill(0);

        let ciphertext = cipher
            .encrypt(&chunk_nonce(chunk_index), &buffer[..length])
            .map_err(|e| anyhow!("encryption error: {}", e))?;

        output_file.write_all(&ciphertext)?;
        chunk_index += 1;
    }

    output_file.flush()?;

    Ok(key.to_vec())
}

pub fn decrypt_file_xchacha20(
    input_file_path: String,
    output_file_path: String,
    key: Vec<u8>,
    padding: usize,
    last_chunk_index: u32,
) -> Result<u8> {
    let input = File::open(input_file_path)?;
    let reader = BufReader::new(input);

    let output = File::create(output_file_path)?;

    decrypt_file_xchacha20_internal(reader, output, key, padding, last_chunk_index)
}

fn decrypt_file_xchacha20_internal<R: Read>(
    mut reader: R,
    mut output_file: File,
    key: Vec<u8>,
    padding: usize,
    last_chunk_index: u32,
) -> Result<u8> {
    if key.len() != KEY_SIZE {
        return Err(anyhow!("the key is {} bytes, not {}", key.len(), KEY_SIZE));
    }
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));

    let mut chunk_index: u32 = 0;

    let mut buffer = vec![0u8; CHUNK_SIZE + TAG_SIZE];

    loop {
        let count = read_chunk(&mut reader, &mut buffer)?;
        if count == 0 {
            break;
        }

        let plaintext = cipher
            .decrypt(&chunk_nonce(chunk_index), &buffer[..count])
            .map_err(|e| anyhow!("decryption error in chunk {}: {}", chunk_index, e))?;

        let length = if chunk_index == last_chunk_index {
            plaintext.len().saturating_sub(padding)
        } else {
            plaintext.len()
        };
        output_file.write_all(&plaintext[..length])?;

        chunk_index += 1;
    }

    output_file.flush()?;

    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 262144;

    fn temp_path() -> String {
        std::env::temp_dir()
            .join(format!("encrypt-test-{}", uuid::Uuid::new_v4()))
            .to_str()
            .unwrap()
            .to_string()
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn last_chunk_index(ciphertext_len: u64) -> u32 {
        (ciphertext_len / (CHUNK as u64 + 16)) as u32
    }

    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let (plain, encrypted, decrypted) = (temp_path(), temp_path(), temp_path());
        std::fs::write(&plain, data).unwrap();
        let key = encrypt_file_xchacha20(plain.clone(), encrypted.clone(), 0).unwrap();
        let size = std::fs::metadata(&encrypted).unwrap().len();
        decrypt_file_xchacha20(
            encrypted.clone(),
            decrypted.clone(),
            key,
            0,
            last_chunk_index(size),
        )
        .unwrap();
        let out = std::fs::read(&decrypted).unwrap();
        for path in [plain, encrypted, decrypted] {
            let _ = std::fs::remove_file(path);
        }
        out
    }

    #[test]
    fn test_files_of_every_size_survive_a_roundtrip() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 5] {
            assert_eq!(roundtrip(&sample(len)), sample(len), "{} bytes", len);
        }
    }

    /// Reads as little as `Read` allows. Network filesystems really do this.
    struct Dribble(std::io::Cursor<Vec<u8>>);
    impl Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(1000);
            self.0.read(&mut buf[..n])
        }
    }

    /// One short read used to end a chunk early, and every chunk after it then
    /// failed to decrypt: the upload was permanently unreadable.
    #[test]
    fn test_short_reads_do_not_change_the_chunk_layout() {
        let data = sample(CHUNK + 5000);
        let (encrypted, decrypted) = (temp_path(), temp_path());
        let key = encrypt_file_xchacha20_internal(
            Dribble(std::io::Cursor::new(data.clone())),
            File::create(&encrypted).unwrap(),
            0,
        )
        .unwrap();
        let size = std::fs::metadata(&encrypted).unwrap().len();
        assert_eq!(size, data.len() as u64 + 32, "two chunks, so two tags");

        decrypt_file_xchacha20_internal(
            Dribble(std::io::Cursor::new(std::fs::read(&encrypted).unwrap())),
            File::create(&decrypted).unwrap(),
            key,
            0,
            last_chunk_index(size),
        )
        .unwrap();
        assert_eq!(std::fs::read(&decrypted).unwrap(), data);
        for path in [encrypted, decrypted] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_padding_is_zeros_and_comes_off_again() {
        let (encrypted, decrypted) = (temp_path(), temp_path());
        // More than one chunk, so the buffer holds old data past the end of the last.
        let data = sample(CHUNK + 10);
        let key = encrypt_file_xchacha20_internal(
            std::io::Cursor::new(data.clone()),
            File::create(&encrypted).unwrap(),
            20,
        )
        .unwrap();
        let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));
        let bytes = std::fs::read(&encrypted).unwrap();
        let mut nonce = XNonce::default();
        nonce[0] = 1;
        let tail = cipher.decrypt(&nonce, &bytes[CHUNK + 16..]).unwrap();
        assert_eq!(&tail[10..], &[0u8; 20]);

        let size = bytes.len() as u64;
        decrypt_file_xchacha20(
            encrypted.clone(),
            decrypted.clone(),
            key,
            20,
            last_chunk_index(size),
        )
        .unwrap();
        assert_eq!(std::fs::read(&decrypted).unwrap(), data);
        for path in [encrypted, decrypted] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_padding_too_big_for_the_chunk_is_an_error_not_a_panic() {
        let encrypted = temp_path();
        let result = encrypt_file_xchacha20_internal(
            std::io::Cursor::new(vec![1u8; CHUNK - 1]),
            File::create(&encrypted).unwrap(),
            2,
        );
        assert!(result.is_err());
        let _ = std::fs::remove_file(encrypted);
    }

    /// All of these used to panic. A bad source has to fail its own job with the
    /// right reason, not take the "internal error" route.
    #[test]
    fn test_bad_input_to_decryption_is_an_error_not_a_panic() {
        let (plain, encrypted, decrypted) = (temp_path(), temp_path(), temp_path());
        std::fs::write(&plain, sample(1000)).unwrap();
        let key = encrypt_file_xchacha20(plain.clone(), encrypted.clone(), 0).unwrap();

        let wrong_key = vec![7u8; 32];
        let short_key = vec![7u8; 5];
        for bad_key in [wrong_key, short_key] {
            assert!(
                decrypt_file_xchacha20(encrypted.clone(), decrypted.clone(), bad_key, 0, 0)
                    .is_err()
            );
        }

        let mut tampered = std::fs::read(&encrypted).unwrap();
        tampered[10] ^= 1;
        std::fs::write(&encrypted, tampered).unwrap();
        assert!(decrypt_file_xchacha20(encrypted.clone(), decrypted.clone(), key, 0, 0).is_err());

        for path in [plain, encrypted, decrypted] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_encrypting_a_missing_file_is_an_error() {
        assert!(encrypt_file_xchacha20(temp_path(), temp_path(), 0).is_err());
    }
}
