/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::{borrow::Cow, collections::BTreeSet};

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use jmap_proto::types::{collection::Collection, property::Property};
use mail_builder::{encoders::base64::base64_encode_mime, mime::make_boundary};
use mail_parser::{decoders::base64::base64_decode, Message, MimeHeaders};
use pgp::{composed, crypto::sym::SymmetricKeyAlgorithm, Deserializable, SignedPublicKey};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use rasn::types::{ObjectIdentifier, OctetString};
use rasn_cms::{
    algorithms::{AES128_CBC, AES256_CBC, RSA},
    pkcs7_compat::EncapsulatedContentInfo,
    AlgorithmIdentifier, EncryptedContent, EncryptedContentInfo, EncryptedKey, EnvelopedData,
    IssuerAndSerialNumber, KeyTransRecipientInfo, RecipientIdentifier, RecipientInfo, CONTENT_DATA,
    CONTENT_ENVELOPED_DATA,
};
use rsa::{pkcs1::DecodeRsaPublicKey, Pkcs1v15Encrypt, RsaPublicKey};
use store::{
    write::{BatchBuilder, ToBitmaps, F_CLEAR, F_VALUE},
    Deserialize, Serialize,
};

use crate::{
    api::{http::ToHttpResponse, HtmlResponse, HttpRequest, HttpResponse},
    auth::oauth::FormData,
    JMAP,
};

const CRYPT_HTML_HEADER: &str = include_str!("../../../../resources/htx/crypto_header.htx");
const CRYPT_HTML_FOOTER: &str = include_str!("../../../../resources/htx/crypto_footer.htx");
const CRYPT_HTML_FORM: &str = include_str!("../../../../resources/htx/crypto_form.htx");
const CRYPT_HTML_SUCCESS: &str = include_str!("../../../../resources/htx/crypto_success.htx");
const CRYPT_HTML_ERROR: &str = include_str!("../../../../resources/htx/crypto_error.htx");

#[derive(Debug)]
pub enum EncryptMessageError {
    AlreadyEncrypted,
    Error(String),
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum Algorithm {
    Aes128,
    Aes256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EncryptionMethod {
    PGP,
    SMIME,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EncryptionParams {
    method: EncryptionMethod,
    algo: Algorithm,
    certs: Vec<Vec<u8>>,
}

#[async_trait::async_trait]
pub trait EncryptMessage {
    async fn encrypt(&self, params: &EncryptionParams) -> Result<Vec<u8>, EncryptMessageError>;
    fn is_encrypted(&self) -> bool;
}

#[async_trait::async_trait]
impl EncryptMessage for Message<'_> {
    async fn encrypt(&self, params: &EncryptionParams) -> Result<Vec<u8>, EncryptMessageError> {
        let root = self.root_part();
        let raw_message = self.raw_message();
        let mut outer_message = Vec::with_capacity((raw_message.len() as f64 * 1.5) as usize);
        let mut inner_message = Vec::with_capacity(raw_message.len());

        // Move MIME headers and body to inner message
        for header in root.headers() {
            (if header.name.is_mime_header() {
                &mut inner_message
            } else {
                &mut outer_message
            })
            .extend_from_slice(&raw_message[header.offset_field()..header.offset_end()]);
        }
        inner_message.extend_from_slice(b"\r\n");
        inner_message.extend_from_slice(&raw_message[root.raw_body_offset()..]);

        // Encrypt inner message
        match params.method {
            EncryptionMethod::PGP => {
                // Prepare encrypted message
                let boundary = make_boundary("_");
                outer_message.extend_from_slice(
                    concat!(
                        "Content-Type: multipart/encrypted;\r\n\t",
                        "protocol=\"application/pgp-encrypted\";\r\n\t",
                        "boundary=\""
                    )
                    .as_bytes(),
                );
                outer_message.extend_from_slice(boundary.as_bytes());
                outer_message.extend_from_slice(
                    concat!(
                        "\"\r\n\r\n",
                        "OpenPGP/MIME message (Automatically encrypted by Stalwart)\r\n\r\n",
                        "--"
                    )
                    .as_bytes(),
                );
                outer_message.extend_from_slice(boundary.as_bytes());
                outer_message.extend_from_slice(
                    concat!(
                        "\r\nContent-Type: application/pgp-encrypted\r\n",
                        "Version: 1\r\n\r\n--"
                    )
                    .as_bytes(),
                );
                outer_message.extend_from_slice(boundary.as_bytes());
                outer_message.extend_from_slice(
                    concat!(
                        "\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\n",
                        "Content-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n"
                    )
                    .as_bytes(),
                );

                // Parse public key
                let mut keys = Vec::with_capacity(params.certs.len());
                for cert in &params.certs {
                    keys.push(SignedPublicKey::from_bytes(&cert[..]).map_err(|err| {
                        EncryptMessageError::Error(format!(
                            "Failed to parse PGP public key: {}",
                            err
                        ))
                    })?);
                }

                // Encrypt contents (TODO: use rayon)
                let algo = params.algo;
                let encrypted_contents = tokio::task::spawn_blocking(move || {
                    composed::message::Message::new_literal_bytes("none", &inner_message)
                        .encrypt_to_keys(
                            &mut StdRng::from_entropy(),
                            match algo {
                                Algorithm::Aes128 => SymmetricKeyAlgorithm::AES128,
                                Algorithm::Aes256 => SymmetricKeyAlgorithm::AES256,
                            },
                            &keys.iter().collect::<Vec<_>>(),
                        )
                        .map_err(|err| {
                            EncryptMessageError::Error(format!(
                                "Failed to encrypt message: {}",
                                err
                            ))
                        })?
                        .to_armored_string(None)
                        .map_err(|err| {
                            EncryptMessageError::Error(format!(
                                "Failed to convert to armored string: {}",
                                err
                            ))
                        })
                })
                .await
                .map_err(|err| {
                    EncryptMessageError::Error(format!("Failed to encrypt message: {}", err))
                })??;
                outer_message.extend_from_slice(encrypted_contents.as_bytes());
                outer_message.extend_from_slice(b"\r\n--");
                outer_message.extend_from_slice(boundary.as_bytes());
                outer_message.extend_from_slice(b"--\r\n");
            }
            EncryptionMethod::SMIME => {
                // Generate random IV
                let mut rng = StdRng::from_entropy();
                let mut iv = vec![0u8; 16];
                rng.fill_bytes(&mut iv);

                // Generate random key
                let mut key = vec![0u8; params.algo.key_size()];
                rng.fill_bytes(&mut key);

                // Encrypt contents (TODO: use rayon)
                let algo = params.algo;
                let (encrypted_contents, key, iv) = tokio::task::spawn_blocking(move || {
                    (algo.encrypt(&key, &iv, &inner_message), key, iv)
                })
                .await
                .map_err(|err| {
                    EncryptMessageError::Error(format!("Failed to encrypt message: {}", err))
                })?;

                // Encrypt key using public keys
                #[allow(clippy::mutable_key_type)]
                let mut recipient_infos = BTreeSet::new();
                for cert in &params.certs {
                    let cert =
                        rasn::der::decode::<rasn_pkix::Certificate>(cert).map_err(|err| {
                            EncryptMessageError::Error(format!(
                                "Failed to parse certificate: {}",
                                err
                            ))
                        })?;

                    let public_key = RsaPublicKey::from_pkcs1_der(
                        cert.tbs_certificate
                            .subject_public_key_info
                            .subject_public_key
                            .as_raw_slice(),
                    )
                    .map_err(|err| {
                        EncryptMessageError::Error(format!("Failed to parse public key: {}", err))
                    })?;
                    let encrypted_key = public_key
                        .encrypt(&mut rng, Pkcs1v15Encrypt, &key[..])
                        .map_err(|err| {
                            EncryptMessageError::Error(format!("Failed to encrypt key: {}", err))
                        })
                        .unwrap();

                    recipient_infos.insert(RecipientInfo::KeyTransRecipientInfo(
                        KeyTransRecipientInfo {
                            version: 0.into(),
                            rid: RecipientIdentifier::IssuerAndSerialNumber(
                                IssuerAndSerialNumber {
                                    issuer: cert.tbs_certificate.issuer,
                                    serial_number: cert.tbs_certificate.serial_number,
                                },
                            ),
                            key_encryption_algorithm: AlgorithmIdentifier {
                                algorithm: RSA.into(),
                                parameters: Some(
                                    rasn::der::encode(&())
                                        .map_err(|err| {
                                            EncryptMessageError::Error(format!(
                                                "Failed to encode RSA algorithm identifier: {}",
                                                err
                                            ))
                                        })?
                                        .into(),
                                ),
                            },
                            encrypted_key: EncryptedKey::from(encrypted_key),
                        },
                    ));
                }

                let pkcs7 = rasn::der::encode(&EncapsulatedContentInfo {
                    content_type: CONTENT_ENVELOPED_DATA.into(),
                    content: Some(
                        rasn::der::encode(&EnvelopedData {
                            version: 0.into(),
                            originator_info: None,
                            recipient_infos,
                            encrypted_content_info: EncryptedContentInfo {
                                content_type: CONTENT_DATA.into(),
                                content_encryption_algorithm: AlgorithmIdentifier {
                                    algorithm: params.algo.to_algorithm_identifier(),
                                    parameters: Some(
                                        rasn::der::encode(&OctetString::from(iv))
                                            .map_err(|err| {
                                                EncryptMessageError::Error(format!(
                                                    "Failed to encode IV: {}",
                                                    err
                                                ))
                                            })?
                                            .into(),
                                    ),
                                },
                                encrypted_content: Some(EncryptedContent::from(encrypted_contents)),
                            },
                            unprotected_attrs: None,
                        })
                        .map_err(|err| {
                            EncryptMessageError::Error(format!(
                                "Failed to encode EnvelopedData: {}",
                                err
                            ))
                        })?
                        .into(),
                    ),
                })
                .map_err(|err| {
                    EncryptMessageError::Error(format!("Failed to encode ContentInfo: {}", err))
                })?;

                // Generate message
                outer_message.extend_from_slice(
                    concat!(
                        "Content-Type: application/pkcs7-mime;\r\n",
                        "\tname=\"smime.p7m\";\r\n",
                        "\tsmime-type=enveloped-data\r\n",
                        "Content-Disposition: attachment;\r\n",
                        "\tfilename=\"smime.p7m\"\r\n",
                        "Content-Transfer-Encoding: base64\r\n\r\n"
                    )
                    .as_bytes(),
                );
                base64_encode_mime(&pkcs7, &mut outer_message, false).map_err(|err| {
                    EncryptMessageError::Error(format!("Failed to base64 encode PKCS7: {}", err))
                })?;
            }
        }

        Ok(outer_message)
    }

    fn is_encrypted(&self) -> bool {
        self.content_type().map_or(false, |ct| {
            let main_type = ct.c_type.as_ref();
            let sub_type = ct
                .c_subtype
                .as_ref()
                .map(|s| s.as_ref())
                .unwrap_or_default();

            (main_type.eq_ignore_ascii_case("application")
                && (sub_type.eq_ignore_ascii_case("pkcs7-mime")
                    || sub_type.eq_ignore_ascii_case("pkcs7-signature")
                    || (sub_type.eq_ignore_ascii_case("octet-stream")
                        && self.attachment_name().map_or(false, |name| {
                            name.rsplit_once('.').map_or(false, |(_, ext)| {
                                ["p7m", "p7s", "p7c", "p7z"].contains(&ext)
                            })
                        }))))
                || (main_type.eq_ignore_ascii_case("multipart")
                    && sub_type.eq_ignore_ascii_case("encrypted"))
        })
    }
}

impl Algorithm {
    fn key_size(&self) -> usize {
        match self {
            Algorithm::Aes128 => 16,
            Algorithm::Aes256 => 32,
        }
    }

    fn to_algorithm_identifier(self) -> ObjectIdentifier {
        match self {
            Algorithm::Aes128 => AES128_CBC.into(),
            Algorithm::Aes256 => AES256_CBC.into(),
        }
    }

    fn encrypt(&self, key: &[u8], iv: &[u8], contents: &[u8]) -> Vec<u8> {
        match self {
            Algorithm::Aes128 => cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
                .encrypt_padded_vec_mut::<Pkcs7>(contents),
            Algorithm::Aes256 => cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into())
                .encrypt_padded_vec_mut::<Pkcs7>(contents),
        }
    }
}

pub fn try_parse_certs(bytes: Vec<u8>) -> Result<(EncryptionMethod, Vec<Vec<u8>>), String> {
    // Check if it's a PEM file
    if let Some(result) = try_parse_pem(&bytes)? {
        Ok(result)
    } else if rasn::der::decode::<rasn_pkix::Certificate>(&bytes[..]).is_ok() {
        Ok((EncryptionMethod::SMIME, vec![bytes]))
    } else if SignedPublicKey::from_bytes(&bytes[..]).is_ok() {
        Ok((EncryptionMethod::PGP, vec![bytes]))
    } else {
        Err("Could not find any valid certificates".to_string())
    }
}

#[allow(clippy::type_complexity)]
fn try_parse_pem(bytes: &[u8]) -> Result<Option<(EncryptionMethod, Vec<Vec<u8>>)>, String> {
    let mut bytes = bytes.iter();
    let mut buf = vec![];
    let mut method = None;
    let mut certs = vec![];

    loop {
        // Find start of PEM block
        for &ch in bytes.by_ref() {
            if ch.is_ascii_whitespace() {
                continue;
            } else if ch == b'-' {
                break;
            } else {
                return Ok(None);
            }
        }

        // Find block type
        for &ch in bytes.by_ref() {
            match ch {
                b'-' => (),
                b'\n' => break,
                _ => {
                    if ch.is_ascii() {
                        buf.push(ch.to_ascii_uppercase());
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
        if buf.is_empty() {
            break;
        }

        // Find type
        let tag = std::str::from_utf8(&buf).unwrap();
        if tag.contains("CERTIFICATE") {
            if method.map_or(false, |m| m == EncryptionMethod::PGP) {
                return Err("Cannot mix PGP and S/MIME certificates".to_string());
            } else {
                method = Some(EncryptionMethod::SMIME);
            }
        } else if tag.contains("PGP") {
            if method.map_or(false, |m| m == EncryptionMethod::SMIME) {
                return Err("Cannot mix PGP and S/MIME certificates".to_string());
            } else {
                method = Some(EncryptionMethod::PGP);
            }
        } else {
            // Ignore block
            let mut found_end = false;
            for &ch in bytes.by_ref() {
                if ch == b'-' {
                    found_end = true;
                } else if ch == b'\n' && found_end {
                    break;
                }
            }
            buf.clear();
            continue;
        }

        // Collect base64
        buf.clear();
        let mut found_end = false;
        for &ch in bytes.by_ref() {
            match ch {
                b'-' => {
                    found_end = true;
                }
                b'\n' => {
                    if found_end {
                        break;
                    }
                }
                _ => {
                    if !ch.is_ascii_whitespace() {
                        buf.push(ch);
                    }
                }
            }
        }

        // Decode base64
        let cert = base64_decode(&buf)
            .ok_or_else(|| "Failed to decode base64 certificate.".to_string())?;
        match method.unwrap() {
            EncryptionMethod::PGP => {
                if let Err(err) = SignedPublicKey::from_bytes(&cert[..]) {
                    return Err(format!("Failed to decode PGP public key: {}", err));
                }
            }
            EncryptionMethod::SMIME => {
                if let Err(err) = rasn::der::decode::<rasn_pkix::Certificate>(&cert) {
                    return Err(format!("Failed to decode X509 certificate: {}", err));
                }
            }
        }
        certs.push(cert);
        buf.clear();
    }

    Ok(method.map(|method| (method, certs)))
}

impl Serialize for EncryptionParams {
    fn serialize(self) -> Vec<u8> {
        let len = bincode::serialized_size(&self).unwrap_or_default();
        let mut buf = Vec::with_capacity(len as usize + 1);
        buf.push(1);
        let _ = bincode::serialize_into(&mut buf, &self);
        buf
    }
}

impl Deserialize for EncryptionParams {
    fn deserialize(bytes: &[u8]) -> store::Result<Self> {
        let version = *bytes.first().ok_or_else(|| {
            store::Error::InternalError(
                "Failed to read version while deserializing encryption params".to_string(),
            )
        })?;
        match version {
            1 if bytes.len() > 1 => bincode::deserialize(&bytes[1..]).map_err(|err| {
                store::Error::InternalError(format!(
                    "Failed to deserialize encryption params: {}",
                    err
                ))
            }),

            _ => Err(store::Error::InternalError(format!(
                "Unknown encryption params version: {}",
                version
            ))),
        }
    }
}

impl ToBitmaps for EncryptionParams {
    fn to_bitmaps(&self, _: &mut Vec<store::write::Operation>, _: u8, _: bool) {
        unreachable!()
    }
}

impl JMAP {
    // Code authorization flow, handles an authorization request
    pub async fn handle_crypto_update(&self, req: &mut HttpRequest) -> HttpResponse {
        let response = match *req.method() {
            hyper::Method::POST => {
                // Parse form
                let form = match FormData::from_request(req, 1024 * 1024).await {
                    Ok(form) => form,
                    Err(err) => return err,
                };

                if let Err(error) = self.validate_form(form).await {
                    let mut response = String::with_capacity(
                        CRYPT_HTML_HEADER.len()
                            + CRYPT_HTML_FOOTER.len()
                            + CRYPT_HTML_ERROR.len()
                            + error.len(),
                    );

                    response.push_str(&CRYPT_HTML_HEADER.replace("@@@", "/crypto"));
                    response.push_str(&CRYPT_HTML_ERROR.replace("@@@", &error));
                    response.push_str(CRYPT_HTML_FOOTER);

                    response
                } else {
                    let mut response = String::with_capacity(
                        CRYPT_HTML_HEADER.len()
                            + CRYPT_HTML_FOOTER.len()
                            + CRYPT_HTML_SUCCESS.len(),
                    );

                    response.push_str(&CRYPT_HTML_HEADER.replace("@@@", "/crypto"));
                    response.push_str(CRYPT_HTML_SUCCESS);
                    response.push_str(CRYPT_HTML_FOOTER);

                    response
                }
            }

            hyper::Method::GET => {
                let mut response = String::with_capacity(
                    CRYPT_HTML_HEADER.len() + CRYPT_HTML_FOOTER.len() + CRYPT_HTML_FORM.len(),
                );

                response.push_str(&CRYPT_HTML_HEADER.replace("@@@", "/crypto"));
                response.push_str(CRYPT_HTML_FORM);
                response.push_str(CRYPT_HTML_FOOTER);

                response
            }
            _ => unreachable!(),
        };

        HtmlResponse::new(response).into_http_response()
    }

    async fn validate_form(&self, mut form: FormData) -> Result<(), Cow<str>> {
        if let (Some(certificate), Some(email), Some(password), Some(encryption)) = (
            form.remove_bytes("certificate"),
            form.get("email"),
            form.get("password"),
            form.get("encryption"),
        ) {
            // Validate fields
            if email.is_empty() || password.is_empty() {
                return Err(Cow::from("Please enter your login and password"));
            } else if encryption != "disable" && certificate.is_empty() {
                return Err(Cow::from("Please select one or more certificates"));
            }

            // Authenticate
            let token = self
                .authenticate_plain(email, password)
                .await
                .ok_or_else(|| Cow::from("Invalid login or password"))?;
            if encryption != "disable" {
                let (method, certs) = try_parse_certs(certificate).map_err(Cow::from)?;
                let algo = match (encryption, method) {
                    ("pgp-256", EncryptionMethod::PGP) => Algorithm::Aes256,
                    ("pgp-128", EncryptionMethod::PGP) => Algorithm::Aes128,
                    ("smime-256", EncryptionMethod::SMIME) => Algorithm::Aes256,
                    ("smime-128", EncryptionMethod::SMIME) => Algorithm::Aes128,
                    _ => {
                        return Err(Cow::from(
                            "No valid certificates found for the selected encryption",
                        ));
                    }
                };
                let params = EncryptionParams {
                    method,
                    algo,
                    certs,
                };

                // Try a test encryption
                if let Err(EncryptMessageError::Error(message)) =
                    Message::parse("Subject: test\r\ntest\r\n".as_bytes())
                        .unwrap()
                        .encrypt(&params)
                        .await
                {
                    return Err(Cow::from(message));
                }

                // Save encryption params
                let mut batch = BatchBuilder::new();
                batch
                    .with_account_id(token.primary_id())
                    .with_collection(Collection::Principal)
                    .update_document(0)
                    .value(Property::Parameters, params, F_VALUE);
                self.write_batch(batch).await.map_err(|_| {
                    Cow::from("Failed to save encryption parameters, please try again later")
                })?;
            } else {
                // Remove encryption params
                let mut batch = BatchBuilder::new();
                batch
                    .with_account_id(token.primary_id())
                    .with_collection(Collection::Principal)
                    .update_document(0)
                    .value(Property::Parameters, (), F_VALUE | F_CLEAR);
                self.write_batch(batch).await.map_err(|_| {
                    Cow::from("Failed to save encryption parameters, please try again later")
                })?;
            }

            Ok(())
        } else {
            Err(Cow::from("Missing form parameters"))
        }
    }
}
