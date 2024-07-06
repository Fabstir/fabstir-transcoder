pub fn create_encrypted_cid(
    cid_type_encrypted: u8,
    encryption_algorithm: u8,
    chunk_size_as_power_of_2: u8,
    encrypted_blob_hash: Vec<u8>,
    encryption_key: Vec<u8>,
    padding: u32,
    original_cid: Vec<u8>,
) -> Vec<u8> {
    let mut result = Vec::new();
    result.push(cid_type_encrypted);
    result.push(encryption_algorithm);
    result.push(chunk_size_as_power_of_2);
    result.extend(encrypted_blob_hash);
    result.extend(encryption_key);
    result.extend(padding.to_be_bytes()); // convert padding to big-endian
    result.extend(original_cid);

    result
}
