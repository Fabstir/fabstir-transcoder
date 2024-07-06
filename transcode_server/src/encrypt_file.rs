use anyhow::{anyhow, Result};
use chacha20poly1305::{
    aead::{generic_array::GenericArray, Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};

pub fn encrypt_file_xchacha20(
    input_file_path: String,
    output_file_path: String,
    padding: usize,
) -> anyhow::Result<Vec<u8>> {
    let input = File::open(input_file_path)?;
    let reader = BufReader::new(input);

    let output = File::create(output_file_path)?;

    let res = encrypt_file_xchacha20_internal(reader, output, padding);

    Ok(res.unwrap())
}

fn encrypt_file_xchacha20_internal<R: Read>(
    mut reader: R,
    mut output_file: File,
    padding: usize,
) -> anyhow::Result<Vec<u8>> {
    //let key = GenericArray::from_slice(&[0u8; 32]);
    let key = XChaCha20Poly1305::generate_key(&mut OsRng);
    let cipher = XChaCha20Poly1305::new(&key);

    let mut chunk_index: u32 = 0;

    let chunk_size = 262144;

    let mut buffer = [0u8; 262144];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        let length = if count < chunk_size {
            count + padding
        } else {
            count
        };

        let mut nonce = XNonce::default();

        let mut foo = [0u8; 24];
        for (place, data) in foo.iter_mut().zip(chunk_index.to_le_bytes().iter()) {
            *place = *data
        }

        nonce.copy_from_slice(&foo);

        let ciphertext = cipher.encrypt(&nonce, &buffer[..length]);

        output_file.write(&ciphertext.unwrap()).unwrap();
        chunk_index = chunk_index + 1;
    }

    output_file.flush().unwrap();

    Ok(key.to_vec())
}

pub fn decrypt_file_xchacha20(
    input_file_path: String,
    output_file_path: String,
    key: Vec<u8>,
    padding: usize,
    last_chunk_index: u32,
) -> anyhow::Result<u8> {
    let input = File::open(input_file_path)?;
    let reader = BufReader::new(input);

    let output = File::create(output_file_path)?;

    println!("let res = decrypt_file_xchacha20_internal(reader, output, key, padding, last_chunk_index);");
    let res = decrypt_file_xchacha20_internal(reader, output, key, padding, last_chunk_index);

    Ok(res.unwrap())
}

fn decrypt_file_xchacha20_internal<R: Read>(
    mut reader: R,
    mut output_file: File,
    key: Vec<u8>,
    padding: usize,
    last_chunk_index: u32,
) -> anyhow::Result<u8> {
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));

    let mut chunk_index: u32 = 0;

    let mut buffer = [0u8; 262160];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        let mut nonce = XNonce::default();

        let mut foo = [0u8; 24];
        for (place, data) in foo.iter_mut().zip(chunk_index.to_le_bytes().iter()) {
            *place = *data
        }

        nonce.copy_from_slice(&foo);

        let ciphertext = cipher.decrypt(&nonce, &buffer[..count]);

        if chunk_index == last_chunk_index {
            output_file
                .write(&ciphertext.unwrap()[..(count - 16 - padding)])
                .unwrap();
        } else {
            output_file.write(&ciphertext.unwrap()).unwrap();
        }

        chunk_index = chunk_index + 1;
    }

    output_file.flush().unwrap();

    Ok(1)
}

fn decrypt_file_xchacha20_internal2<R: Read>(
    mut reader: R,
    mut output_file: File,
    key: Vec<u8>,
    padding: usize,
    last_chunk_index: u32,
) -> anyhow::Result<u8> {
    println!("decrypt_file_xchacha20_internal key: {:?}", key);
    println!("let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));");
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));

    println!("let mut chunk_index: u32 = 0;");
    let mut chunk_index: u32 = 0;

    println!("let mut buffer = [0u8; 262160];");
    let mut buffer = [0u8; 262160];

    println!("loop");
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        println!("count: {}", &count);

        println!("First 32 bytes of buffer: {}", hex::encode(&buffer[..32]));

        println!("let mut nonce = XNonce::default();");
        let mut nonce = XNonce::default();

        let mut foo = [0u8; 24];
        println!("for (place, data) in foo.iter_mut().zip(chunk_index.to_le_bytes().iter())");
        for (place, data) in foo.iter_mut().zip(chunk_index.to_le_bytes().iter()) {
            *place = *data
        }

        println!("nonce.copy_from_slice(&foo);");
        nonce.copy_from_slice(&foo);

        //        let ciphertext = cipher.decrypt(&nonce, &buffer[..count]);
        let ciphertext = cipher.decrypt(&nonce, &buffer[..count]);
        match ciphertext {
            Ok(ciphertext) => {
                if chunk_index == last_chunk_index {
                    output_file
                        .write(&ciphertext[..(count - 16 - padding)])
                        .unwrap();

                //                    output_file.write_all(&ciphertext[..(count - 16 - padding)])?;
                } else {
                    println!("output_file.write(&ciphertext.unwrap()).unwrap();");
                    output_file.write(&ciphertext).unwrap();
                    //        output_file.write_all(&ciphertext)?;
                    println!("after output_file.write(&ciphertext.unwrap()).unwrap();");
                }
            }
            Err(e) => return Err(anyhow!("encryption error: {}", e)),
        }

        println!("if chunk_index == last_chunk_index");
        // if chunk_index == last_chunk_index {
        //     output_file
        //         .write(&ciphertext.unwrap()[..(count - 16 - padding)])
        //         .unwrap();
        // } else {
        //     println!("output_file.write(&ciphertext.unwrap()).unwrap();");
        //                output_file.write(&ciphertext.unwrap()).unwrap();
        //     println!("after output_file.write(&ciphertext.unwrap()).unwrap();");
        // }

        // if chunk_index == last_chunk_index {
        //     if let Err(e) =
        //         output_file.write_all(&ciphertext.as_ref().unwrap()[..(count - 16 - padding)])
        //     {
        //         return Err(anyhow!("write error: {}", e));
        //     }
        // } else {
        //     println!("output_file.write(&ciphertext.unwrap()).unwrap();");
        //     if let Err(e) = output_file.write_all(ciphertext.as_ref().unwrap()) {
        //         return Err(anyhow!("write error: {}", e));
        //     }
        //     println!("after output_file.write(&ciphertext.unwrap()).unwrap();");
        // }

        println!("chunk_index = chunk_index + 1;");
        chunk_index = chunk_index + 1;
    }

    output_file.flush().unwrap();

    Ok(1)
}
