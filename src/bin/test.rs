use base64::{engine::general_purpose, Engine as _};
use rmp_serde::{decode, encode};
use serde_json::json;

fn main() {
    // Serialize a JSON payload using MessagePack (binary data).
    let payload = json!({ "value": "Hello World" });
    let serialized = encode::to_vec(&payload).unwrap();

    // Convert the binary data to a Base64 encoded string.
    let b64_string = general_purpose::STANDARD.encode(&serialized);
    println!("Base64 Encoded: {}", b64_string);

    // Later, decode the Base64 string back to a Vec<u8>
    let decoded_bytes = general_purpose::STANDARD
        .decode(&b64_string)
        .expect("Failed to decode Base64 data");

    // Now, decode the MessagePack binary data into a serde_json::Value.
    let deserialized: serde_json::Value = decode::from_slice(&decoded_bytes).unwrap();
    println!("Deserialized value: {}", deserialized);
}