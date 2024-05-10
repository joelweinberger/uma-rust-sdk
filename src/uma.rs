use std::fmt;

use bitcoin::secp256k1::{
    ecdsa::Signature, hashes::sha256, Message, PublicKey, Secp256k1, SecretKey,
};
use rand_core::{OsRng, RngCore};

use crate::protocol::{
    counter_party_data::CounterPartyDataOptions,
    currency::Currency,
    kyc_status::KycStatus,
    lnurl_request::{LnurlpRequest, UmaLnurlpRequest},
    lnurl_response::{LnurlComplianceResponse, LnurlpResponse},
    pay_request::PayRequest,
    payer_data::{CompliancePayerData, PayerData, TravelRuleFormat},
    payreq_response::{PayReqResponse, PayReqResponseCompliance, PayReqResponsePaymentInfo},
    pub_key_response::PubKeyResponse,
};

use crate::{
    public_key_cache,
    version::{self, is_version_supported},
};

#[derive(Debug)]
pub enum Error {
    Secp256k1Error(bitcoin::secp256k1::Error),
    EciesSecp256k1Error(ecies::SecpError),
    SignatureFormatError,
    InvalidSignature,
    InvalidResponse,
    ProtocolError(crate::protocol::Error),
    MissingUrlParam(String),
    InvalidUrlPath,
    InvalidHost,
    InvalidData(serde_json::Error),
    CreateInvoiceError(String),
    InvalidUMAAddress,
    InvalidVersion,
    UnsupportedVersion,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Secp256k1Error(err) => write!(f, "Secp256k1 error {}", err),
            Self::EciesSecp256k1Error(err) => write!(f, "Ecies Secp256k1 error {}", err),
            Self::SignatureFormatError => write!(f, "Signature format error"),
            Self::InvalidSignature => write!(f, "Invalid signature"),
            Self::InvalidResponse => write!(f, "Invalid response"),
            Self::ProtocolError(err) => write!(f, "Protocol error {}", err),
            Self::MissingUrlParam(param) => write!(f, "Missing URL param {}", param),
            Self::InvalidUrlPath => write!(f, "Invalid URL path"),
            Self::InvalidHost => write!(f, "Invalid host"),
            Self::InvalidData(err) => write!(f, "Invalid data {}", err),
            Self::CreateInvoiceError(err) => write!(f, "Create invoice error {}", err),
            Self::InvalidUMAAddress => write!(f, "Invalid UMA address"),
            Self::InvalidVersion => write!(f, "Invalid version"),
            Self::UnsupportedVersion => write!(f, "Unsupported version"),
        }
    }
}

/// Fetches the public key for another VASP.
///
/// If the public key is not in the cache, it will be fetched from the VASP's domain.
///     The public key will be cached for future use.
///
/// # Arguments
///
/// * `vasp_domain` - the domain of the VASP.
/// * `cache` - the PublicKeyCache cache to use. You can use the InMemoryPublicKeyCache struct, or implement your own persistent cache with any storage type.
pub fn fetch_public_key_for_vasp<T>(
    vasp_domain: &str,
    public_key_cache: &mut T,
) -> Result<PubKeyResponse, Error>
where
    T: public_key_cache::PublicKeyCache,
{
    let publick_key = public_key_cache.fetch_public_key_for_vasp(vasp_domain);
    if let Some(public_key) = publick_key {
        return Ok(public_key.clone());
    }

    let scheme = match vasp_domain.starts_with("localhost:") {
        true => "http",
        false => "https",
    };

    let url = format!("{}//{}/.well-known/lnurlpubkey", scheme, vasp_domain);
    let response = reqwest::blocking::get(url).map_err(|_| Error::InvalidResponse)?;

    if !response.status().is_success() {
        return Err(Error::InvalidResponse);
    }

    let bytes = response.bytes().map_err(|_| Error::InvalidResponse)?;

    let pubkey_response: PubKeyResponse =
        serde_json::from_slice(&bytes).map_err(Error::InvalidData)?;

    public_key_cache.add_public_key_for_vasp(vasp_domain, &pubkey_response);
    Ok(pubkey_response)
}

pub fn generate_nonce() -> String {
    OsRng.next_u64().to_string()
}

fn sign_payload(payload: &[u8], private_key_bytes: &[u8]) -> Result<String, Error> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(private_key_bytes).map_err(Error::Secp256k1Error)?;
    let msg = Message::from_hashed_data::<sha256::Hash>(payload);
    let signature = secp.sign_ecdsa(&msg, &sk);
    let sig_string = hex::encode(signature.serialize_der());
    Ok(sig_string)
}

fn verify_ecdsa(payload: &[u8], signature: &str, pub_key_bytes: &[u8]) -> Result<(), Error> {
    let sig_bytes = hex::decode(signature).map_err(|_| Error::SignatureFormatError)?;
    let secp = Secp256k1::new();
    let msg = Message::from_hashed_data::<sha256::Hash>(payload);
    let sig = Signature::from_der(&sig_bytes).map_err(Error::Secp256k1Error)?;
    let pk = PublicKey::from_slice(pub_key_bytes).map_err(Error::Secp256k1Error)?;
    secp.verify_ecdsa(&msg, &sig, &pk)
        .map_err(|_| Error::InvalidSignature)
}

/// Verifies the signature on a uma pay request based on the public key of the VASP making the request.
///
/// # Arguments
///
/// * `pay_req` - the signed query to verify.
/// * `other_vasp_pub_key` - the bytes of the signing public key of the VASP making this request.
pub fn verify_pay_req_signature(
    pay_req: &PayRequest,
    other_vasp_pub_key: &[u8],
) -> Result<(), Error> {
    let payload = pay_req.signable_payload().map_err(Error::ProtocolError)?;
    verify_ecdsa(
        &payload,
        &pay_req
            .clone()
            .payer_data
            .ok_or(Error::InvalidSignature)?
            .compliance()
            .map_err(Error::ProtocolError)?
            .ok_or(Error::ProtocolError(
                crate::protocol::Error::MissingPayerDataCompliance,
            ))?
            .signature,
        other_vasp_pub_key,
    )
}

/// Creates a signed uma request URL.
///
/// # Arguments
///
/// * `signing_private_key` - the private key of the VASP that is sending the payment. This will be used to sign the request.
/// * `receiver_address` - the address of the receiver of the payment (i.e. $bob@vasp2).
/// * `sender_vasp_domain` - the domain of the VASP that is sending the payment. It will be used by the receiver to fetch the public keys of the sender.
/// * `is_subject_to_travel_rule` - whether the sending VASP is a financial institution that requires travel rule information.
/// * `uma_version_override` - the version of the UMA protocol to use. If not specified, the latest version will be used.
pub fn get_signed_lnurlp_request_url(
    signing_private_key: &[u8],
    receiver_address: &str,
    sender_vasp_domain: &str,
    is_subject_to_travel_rule: bool,
    uma_version_override: Option<&str>,
) -> Result<url::Url, Error> {
    let nonce = generate_nonce();
    let uma_version = match uma_version_override {
        Some(version) => version.to_string(),
        None => version::uma_protocol_version(),
    };
    let mut unsigned_request = LnurlpRequest {
        receiver_address: receiver_address.to_owned(),
        nonce: Some(nonce),
        timestamp: Some(chrono::Utc::now().timestamp()),
        signature: None,
        vasp_domain: Some(sender_vasp_domain.to_owned()),
        is_subject_to_travel_rule: Some(is_subject_to_travel_rule),
        uma_version: Some(uma_version),
    };

    let sig = sign_payload(
        &unsigned_request
            .signable_payload()
            .map_err(Error::ProtocolError)?,
        signing_private_key,
    )?;
    unsigned_request.signature = Some(sig);

    unsigned_request
        .encode_to_url()
        .map_err(Error::ProtocolError)
}

/// Checks if the given URL is a valid UMA request.
pub fn is_uma_lnurl_query(url: &url::Url) -> bool {
    parse_lnurlp_request(url).is_ok()
}

/// Parses the message into an LnurlpRequest object.
///
/// # Arguments
/// * `url` - the full URL of the uma request.
pub fn parse_lnurlp_request(url: &url::Url) -> Result<LnurlpRequest, Error> {
    let mut query = url.query_pairs();
    let signature = query
        .find(|(key, _)| key == "signature")
        .map(|(_, value)| value)
        .ok_or(Error::MissingUrlParam("signature".to_string()))?;

    let mut query = url.query_pairs();
    let vasp_domain = query
        .find(|(key, _)| key == "vaspDomain")
        .map(|(_, value)| value)
        .ok_or(Error::MissingUrlParam("vsapDomain".to_string()))?;

    let mut query = url.query_pairs();
    let nonce = query
        .find(|(key, _)| key == "nonce")
        .map(|(_, value)| value)
        .ok_or(Error::MissingUrlParam("nonce".to_string()))?;

    let mut query = url.query_pairs();
    let is_subject_to_travel_rule = query
        .find(|(key, _)| key == "isSubjectToTravelRule")
        .map(|(_, value)| value.to_lowercase() == "true")
        .unwrap_or(false);

    let mut query = url.query_pairs();
    let timestamp = query
        .find(|(key, _)| key == "timestamp")
        .map(|(_, value)| value.parse::<i64>())
        .ok_or(Error::MissingUrlParam("timestamp".to_string()))?
        .map_err(|_| Error::MissingUrlParam("timestamp".to_string()))?;

    let mut query = url.query_pairs();
    let uma_version = query
        .find(|(key, _)| key == "umaVersion")
        .map(|(_, value)| value)
        .ok_or(Error::MissingUrlParam("umaVersion".to_string()))?;

    let path_parts: Vec<&str> = url.path_segments().ok_or(Error::InvalidUrlPath)?.collect();
    if path_parts.len() != 3 || path_parts[0] != ".well-known" || path_parts[1] != "lnurlp" {
        return Err(Error::InvalidUrlPath);
    }

    if !is_version_supported(&uma_version) {
        return Err(Error::UnsupportedVersion);
    }

    let receiver_address = format!(
        "{}@{}",
        path_parts[2],
        url.host_str().ok_or(Error::InvalidHost)?
    );

    Ok(LnurlpRequest {
        receiver_address,
        vasp_domain: Some(vasp_domain.to_string()),
        signature: Some(signature.to_string()),
        nonce: Some(nonce.to_string()),
        timestamp: Some(timestamp),
        is_subject_to_travel_rule: Some(is_subject_to_travel_rule),
        uma_version: Some(uma_version.to_string()),
    })
}

/// Verifies the signature on an uma Lnurlp query based on the public key of the VASP making the request.
///
/// # Arguments
/// * `query` - the signed query to verify.
/// * `other_vasp_pub_key` - the bytes of the signing public key of the VASP making this request.
pub fn verify_uma_lnurlp_query_signature(
    query: UmaLnurlpRequest,
    other_vasp_pub_key: &[u8],
) -> Result<(), Error> {
    verify_ecdsa(
        &query.signable_payload(),
        &query.signature,
        other_vasp_pub_key,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn get_lnurlp_response(
    query: &LnurlpRequest,
    private_key_bytes: &[u8],
    requires_travel_rule_info: bool,
    callback: &str,
    encoded_metadata: &str,
    min_sendable_sats: i64,
    max_sendable_sats: i64,
    payer_data_options: &CounterPartyDataOptions,
    currency_options: &[Currency],
    receiver_kyc_status: KycStatus,
    comment_chars_allowed: Option<i64>,
    nostr_pubkey: Option<String>,
) -> Result<LnurlpResponse, Error> {
    let compliance_response = get_signed_compliance_respionse(
        query,
        private_key_bytes,
        requires_travel_rule_info,
        receiver_kyc_status,
    )?;
    let uma_version = version::select_lower_version(
        &query.uma_version.clone().ok_or(Error::InvalidVersion)?,
        &version::uma_protocol_version(),
    )
    .map_err(|_| Error::InvalidVersion)?;

    let mut allows_nostr: Option<bool> = None;
    if nostr_pubkey.is_some() {
        allows_nostr = Some(true);
    }

    Ok(LnurlpResponse {
        tag: "payRequest".to_string(),
        callback: callback.to_string(),
        min_sendable: min_sendable_sats * 1000,
        max_sendable: max_sendable_sats * 1000,
        encoded_metadata: encoded_metadata.to_string(),
        currencies: Some(currency_options.to_vec()),
        required_payer_data: Some(payer_data_options.clone()),
        compliance: Some(compliance_response.clone()),
        uma_version: Some(uma_version.clone()),
        comment_chars_allowed,
        nostr_pubkey,
        allows_nostr,
    })
}

fn get_signed_compliance_respionse(
    query: &LnurlpRequest,
    private_key_bytes: &[u8],
    is_subject_to_travel_rule: bool,
    receiver_kyc_status: KycStatus,
) -> Result<LnurlComplianceResponse, Error> {
    let timestamp = chrono::Utc::now().timestamp();
    let nonce = generate_nonce();
    let payload_string = format!("{}|{}|{}", query.receiver_address, nonce, timestamp);

    let signature = sign_payload(payload_string.as_bytes(), private_key_bytes)?;

    Ok(LnurlComplianceResponse {
        kyc_status: receiver_kyc_status,
        signature,
        nonce,
        timestamp,
        is_subject_to_travel_rule,
        receiver_identifier: query.receiver_address.clone(),
    })
}

/// Verifies the signature on an uma Lnurlp response based on the public key of the VASP making the request.
///
/// # Arguments
/// * `response` - the signed response to verify.
/// * `other_vasp_pub_key` - the bytes of the signing public key of the VASP making this request.
pub fn verify_uma_lnurlp_response_signature(
    response: &LnurlpResponse,
    other_vasp_pub_key: &[u8],
) -> Result<(), Error> {
    let uma_response = response.as_uma_response().ok_or(Error::InvalidResponse)?;
    let payload = uma_response.signable_payload();
    verify_ecdsa(
        &payload,
        &uma_response.compliance.signature,
        other_vasp_pub_key,
    )
}

pub fn parse_lnurlp_response(bytes: &[u8]) -> Result<LnurlpResponse, Error> {
    serde_json::from_slice(bytes).map_err(Error::InvalidData)
}

/// Gets the domain of the VASP from an uma address.
pub fn get_vasp_domain_from_uma_address(uma_address: &str) -> Result<String, Error> {
    let address_parts: Vec<&str> = uma_address.split('@').collect();
    if address_parts.len() != 2 {
        Err(Error::InvalidUMAAddress)
    } else {
        Ok(address_parts[1].to_string())
    }
}

/// Creates a signed uma pay request.
///
/// # Arguments
/// * `receiver_encryption_pub_key` - the public key of the receiver of the payment. This will be used to encrypt the travel rule information.
/// * `sending_vasp_private_key` - the private key of the VASP that is sending the payment. This will be used to sign the request.
/// * `currency_code` - the currency code of the payment.
/// * `amount` - the amount of the payment in the smallest unit of the specified currency (i.e. cents for USD).
/// * `payer_identifier` - the identifier of the sender. For example, $alice@vasp1.com
/// * `payer_name` - the name of the sender.
/// * `payer_email` - the email of the sender.
/// * `tr_info` - the travel rule information to be encrypted.
/// * `travel_rule_format` - the format of the travel rule information (e.g. IVMS). Null indicates
///     raw json or a custom format. This field is formatted as <standardized format>@<version>
///     (e.g. ivms@101.2023). Version is optional.
/// * `payer_kyc_status` - the KYC status of the sender.
/// * `payer_uxtos` - the list of UTXOs of the sender's channels that might be used to fund the payment.
/// * `payer_node_pubkey` - If known, the public key of the sender's node. If supported by the receiving VASP's compliance provider, this will be used to pre-screen the sender's UTXOs for compliance purposes.
/// * `utxo_callback` - the URL that the receiver will use to fetch the sender's UTXOs.
#[allow(clippy::too_many_arguments)]
pub fn get_pay_request(
    amount: i64,
    receiver_encryption_pub_key: &[u8],
    sending_vasp_private_key: &[u8],
    receving_currency_code: &str,
    is_amount_in_receving_currency_code: bool,
    payer_identifier: &str,
    uma_major_version: i32,
    payer_name: Option<&str>,
    payer_email: Option<&str>,
    tr_info: Option<&str>,
    travel_rule_format: Option<TravelRuleFormat>,
    payer_kyc_status: KycStatus,
    payer_uxtos: &[String],
    payer_node_pubkey: Option<&str>,
    utxo_callback: &str,
    requested_payee_data: Option<CounterPartyDataOptions>,
    comment: Option<&str>,
) -> Result<PayRequest, Error> {
    let compliance_data = get_signed_compliance_payer_data(
        receiver_encryption_pub_key,
        sending_vasp_private_key,
        payer_identifier,
        tr_info,
        travel_rule_format,
        payer_kyc_status,
        payer_uxtos,
        payer_node_pubkey,
        utxo_callback,
    )?;

    let sending_amount_currency_code = if is_amount_in_receving_currency_code {
        Some(receving_currency_code.to_string())
    } else {
        None
    };

    let payer_data = PayerData(serde_json::json!({
        "identifier": payer_identifier,
        "name": payer_name,
        "email": payer_email,
        "compliance": compliance_data,
    }));
    Ok(PayRequest {
        sending_amount_currency_code,
        receiving_currency_code: Some(receving_currency_code.to_string()),
        payer_data: Some(payer_data),
        comment: comment.map(|s| s.to_string()),
        uma_major_version,
        amount,
        requested_payee_data,
    })
}

#[allow(clippy::too_many_arguments)]
fn get_signed_compliance_payer_data(
    receiver_encryption_pub_key: &[u8],
    sending_vasp_private_key: &[u8],
    payer_identifier: &str,
    tr_info: Option<&str>,
    travel_rule_format: Option<TravelRuleFormat>,
    payer_kyc_status: KycStatus,
    payer_uxtos: &[String],
    payer_node_pubkey: Option<&str>,
    utxo_callback: &str,
) -> Result<CompliancePayerData, Error> {
    let timestamp = chrono::Utc::now().timestamp();
    let nonce = generate_nonce();

    let encrypted_tr_info = match tr_info {
        Some(tr_info) => Some(encrypt_tr_info(tr_info, receiver_encryption_pub_key)?),
        None => None,
    };
    let payload_string = format!("{}|{}|{}", payer_identifier, nonce, timestamp);
    let signature = sign_payload(payload_string.as_bytes(), sending_vasp_private_key)?;

    Ok(CompliancePayerData {
        utxos: payer_uxtos.to_vec(),
        node_pubkey: payer_node_pubkey.map(|s| s.to_string()),
        kyc_status: payer_kyc_status,
        encrypted_travel_rule_info: encrypted_tr_info,
        travel_rule_format,
        signature,
        signature_nonce: nonce,
        signature_timestamp: timestamp,
        utxo_callback: utxo_callback.to_string(),
    })
}

fn encrypt_tr_info(tr_info: &str, receiver_encryption_pub_key: &[u8]) -> Result<String, Error> {
    let cipher_text = ecies::encrypt(receiver_encryption_pub_key, tr_info.as_bytes())
        .map_err(Error::EciesSecp256k1Error)?;
    Ok(hex::encode(cipher_text))
}

pub fn parse_pay_request(bytes: &[u8]) -> Result<PayRequest, Error> {
    serde_json::from_slice(bytes).map_err(Error::InvalidData)
}

pub trait UmaInvoiceCreator {
    fn create_uma_invoice(
        &self,
        amount_msat: i64,
        metadata: &str,
    ) -> Result<String, Box<dyn std::error::Error>>;
}

#[allow(clippy::too_many_arguments)]
pub fn get_pay_req_response<T>(
    query: &PayRequest,
    invoice_creator: &T,
    metadata: &str,
    currency_code: &str,
    currency_decimals: i32,
    conversion_rate: f64,
    receiver_fees_millisats: i64,
    receiver_channel_utxos: &[String],
    receiver_node_pub_key: Option<&str>,
    utxo_callback: &str,
) -> Result<PayReqResponse, Error>
where
    T: UmaInvoiceCreator,
{
    let msats_amount =
        (query.amount as f64 * conversion_rate).round() as i64 + receiver_fees_millisats;
    let encoded_payer_data =
        serde_json::to_string(&query.payer_data).map_err(Error::InvalidData)?;
    let encoded_invoice = invoice_creator
        .create_uma_invoice(
            msats_amount,
            &format!("{}{{{}}}", metadata, encoded_payer_data),
        )
        .map_err(|e| Error::CreateInvoiceError(e.to_string()))?;

    Ok(PayReqResponse {
        encoded_invoice,
        routes: [].to_vec(),
        compliance: PayReqResponseCompliance {
            node_pub_key: receiver_node_pub_key.map(|s| s.to_string()),
            utxos: receiver_channel_utxos.to_vec(),
            utxo_callback: utxo_callback.to_string(),
        },
        payment_info: PayReqResponsePaymentInfo {
            currency_code: currency_code.to_string(),
            decimals: currency_decimals,
            multiplier: conversion_rate,
            exchange_fees_millisatoshi: receiver_fees_millisats,
        },
    })
}

pub fn parse_pay_req_response(bytes: &[u8]) -> Result<PayReqResponse, Error> {
    serde_json::from_slice(bytes).map_err(Error::InvalidData)
}
