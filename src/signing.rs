//! EIP-712 order signing and HMAC L2 auth for Polymarket CLOB.

use anyhow::{Context, Result};
use base64::Engine;
use ethers::types::{H160, U256};
use ethers::utils::keccak256;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const PROTOCOL_NAME: &str = "Polymarket CTF Exchange";
const PROTOCOL_VERSION: &str = "1";

/// Polygon mainnet CTF Exchange (non-neg-risk).
pub const EXCHANGE_ADDRESS_POLYGON: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
/// Neg-risk CTF Exchange (multi-outcome markets).
pub const NEG_RISK_EXCHANGE_POLYGON: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

fn u256_to_32_bytes(u: U256) -> [u8; 32] {
    let mut buf = [0u8; 32];
    u.to_big_endian(&mut buf);
    buf
}

fn address_to_32_bytes(addr: &H160) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[12..32].copy_from_slice(addr.as_bytes());
    buf
}

/// EIP-712 domain separator hash for Polymarket CTF Exchange.
fn domain_separator(verifying_contract: H160, chain_id: u64) -> [u8; 32] {
    let type_hash = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak256(PROTOCOL_NAME.as_bytes());
    let version_hash = keccak256(PROTOCOL_VERSION.as_bytes());
    let mut encoded = Vec::with_capacity(32 * 5);
    encoded.extend_from_slice(&type_hash);
    encoded.extend_from_slice(&name_hash);
    encoded.extend_from_slice(&version_hash);
    encoded.extend_from_slice(&u256_to_32_bytes(U256::from(chain_id)));
    encoded.extend_from_slice(&address_to_32_bytes(&verifying_contract));
    keccak256(encoded)
}

/// Order struct type hash (must match Polymarket exchange).
fn order_type_hash() -> [u8; 32] {
    keccak256(
        "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)",
    )
}

/// Build EIP-712 struct hash for the order.
fn order_struct_hash(
    salt: U256,
    maker: H160,
    signer: H160,
    taker: H160,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
) -> [u8; 32] {
    let type_hash = order_type_hash();
    let mut encoded = Vec::with_capacity(32 * 13);
    encoded.extend_from_slice(&type_hash);
    encoded.extend_from_slice(&u256_to_32_bytes(salt));
    encoded.extend_from_slice(&address_to_32_bytes(&maker));
    encoded.extend_from_slice(&address_to_32_bytes(&signer));
    encoded.extend_from_slice(&address_to_32_bytes(&taker));
    encoded.extend_from_slice(&u256_to_32_bytes(token_id));
    encoded.extend_from_slice(&u256_to_32_bytes(maker_amount));
    encoded.extend_from_slice(&u256_to_32_bytes(taker_amount));
    encoded.extend_from_slice(&u256_to_32_bytes(expiration));
    encoded.extend_from_slice(&u256_to_32_bytes(nonce));
    encoded.extend_from_slice(&u256_to_32_bytes(fee_rate_bps));
    encoded.extend_from_slice(&u256_to_32_bytes(U256::from(side)));
    encoded.extend_from_slice(&u256_to_32_bytes(U256::from(signature_type)));
    keccak256(encoded)
}

/// EIP-712 digest for signing: keccak256("\x19\x01" || domain_sep || struct_hash).
fn eip712_digest(domain_sep: [u8; 32], struct_hash: [u8; 32]) -> [u8; 32] {
    let mut prefixed = Vec::with_capacity(2 + 32 + 32);
    prefixed.extend_from_slice(b"\x19\x01");
    prefixed.extend_from_slice(&domain_sep);
    prefixed.extend_from_slice(&struct_hash);
    keccak256(prefixed)
}

/// Parse token_id string (hex 0x... or decimal) to U256.
pub fn parse_token_id(token_id: &str) -> Result<U256> {
    let s = token_id.trim().trim_start_matches("0x");
    if token_id.starts_with("0x") || token_id.starts_with("0X") {
        U256::from_str_radix(s, 16).context("token_id hex parse")
    } else {
        U256::from_dec_str(token_id).context("token_id decimal parse")
    }
}

/// Sign an order with the wallet; returns 0x-prefixed hex signature.
pub async fn sign_order(
    wallet: &ethers::signers::LocalWallet,
    chain_id: u64,
    verifying_contract: H160,
    salt: u64,
    maker: H160,
    signer: H160,
    taker: H160,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: u64,
    nonce: u64,
    fee_rate_bps: u64,
    side: u8,
    signature_type: u8,
) -> Result<String> {
    let domain_sep = domain_separator(verifying_contract, chain_id);
    let struct_hash = order_struct_hash(
        U256::from(salt),
        maker,
        signer,
        taker,
        token_id,
        maker_amount,
        taker_amount,
        U256::from(expiration),
        U256::from(nonce),
        U256::from(fee_rate_bps),
        side,
        signature_type,
    );
    let digest = eip712_digest(domain_sep, struct_hash);
    let sig = wallet.sign_hash(ethers::types::H256::from(digest))?;
    let sig_bytes = sig.to_vec();
    Ok(format!("0x{}", hex::encode(sig_bytes)))
}

/// Build POLY_SIGNATURE for L2: HMAC-SHA256(secret, timestamp + method + path + body), base64url.
pub fn build_poly_hmac(
    secret_b64: &str,
    timestamp: u64,
    method: &str,
    request_path: &str,
    body: Option<&str>,
) -> Result<String> {
    let message = if let Some(b) = body {
        format!("{}{}{}{}", timestamp, method, request_path, b)
    } else {
        format!("{}{}{}", timestamp, method, request_path)
    };
    let secret_bytes = base64::engine::general_purpose::STANDARD
        .decode(
            secret_b64
                .replace('-', "+")
                .replace('_', "/")
                .trim()
                .as_bytes(),
        )
        .context("SECRET base64 decode")?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&secret_bytes).context("HMAC key")?;
    mac.update(message.as_bytes());
    let result = mac.finalize();
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(result.into_bytes());
    let sig_url_safe = sig_b64.replace('+', "-").replace('/', "_");
    Ok(sig_url_safe)
}
