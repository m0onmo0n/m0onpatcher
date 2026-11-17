use sha1::{Digest, Sha1};

use crate::encoding::url_encode_file_name;
use crate::game_data::PASSAGE_EXTENSION;

pub fn passage_name_truncated_hash(url_encoded_passage_name: &str) -> String {
    let digest = Sha1::digest(url_encoded_passage_name.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn include_hash_in_passage_file_name(url_encoded_name_without_extension: &str) -> String {
    let hash = passage_name_truncated_hash(url_encoded_name_without_extension);
    format!("{url_encoded_name_without_extension}_sha-{hash}")
}

pub fn is_valid_hash_name_match(file_name: &str, passage_name: &str) -> bool {
    if !file_name.contains("_sha-") {
        return false;
    }
    let url_encoded = url_encode_file_name(passage_name);
    let hash = passage_name_truncated_hash(&url_encoded);
    let expected = format!("{url_encoded}_sha-{hash}.{PASSAGE_EXTENSION}");
    file_name == expected
}
