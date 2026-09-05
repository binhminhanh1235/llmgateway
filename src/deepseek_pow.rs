use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::Semaphore, task, time::timeout};

use std::time::Duration;

const RATE_BYTES: usize = 136;
const OUTPUT_BYTES: usize = 32;
const DOMAIN_SUFFIX: u8 = 0x06;
const ROUNDS: usize = 23;
const MAX_DIFFICULTY: u64 = 250_000;
const MAX_SALT_LEN: usize = 1_024;
const COMPLETION_PATH: &str = "/api/v0/chat/completion";
static POW_SLOTS: Semaphore = Semaphore::const_new(2);

const ROTATION_OFFSETS: [u32; 25] = [
    0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61,
    56, 14,
];

const ROUND_CONSTANTS: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

#[derive(Clone, Debug, Deserialize)]
pub struct DeepSeekPowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: u64,
    pub expire_at: u64,
    #[serde(default)]
    pub target_path: String,
}

impl DeepSeekPowChallenge {
    fn validate(&self) -> Result<[u8; OUTPUT_BYTES], String> {
        if self.algorithm != "DeepSeekHashV1" {
            return Err(format!(
                "unsupported DeepSeek PoW algorithm '{}'",
                self.algorithm
            ));
        }
        if self.salt.is_empty() || self.salt.len() > MAX_SALT_LEN {
            return Err(format!(
                "DeepSeek PoW salt must contain 1-{MAX_SALT_LEN} bytes"
            ));
        }
        if !(1..=MAX_DIFFICULTY).contains(&self.difficulty) {
            return Err(format!(
                "DeepSeek PoW difficulty must be between 1 and {MAX_DIFFICULTY}"
            ));
        }
        if !self.target_path.is_empty() && self.target_path != COMPLETION_PATH {
            return Err(format!(
                "unexpected DeepSeek PoW target path '{}'",
                self.target_path
            ));
        }
        decode_digest(&self.challenge)
    }

    pub fn response_header(&self, answer: u64) -> Result<String, String> {
        self.validate()?;
        if answer >= self.difficulty {
            return Err("DeepSeek PoW answer is outside the challenge search range".into());
        }
        let target_path = if self.target_path.is_empty() {
            COMPLETION_PATH
        } else {
            self.target_path.as_str()
        };
        let payload = serde_json::to_vec(&json!({
            "algorithm": self.algorithm,
            "challenge": self.challenge,
            "salt": self.salt,
            "answer": answer,
            "signature": self.signature,
            "target_path": target_path
        }))
        .map_err(|error| error.to_string())?;
        Ok(STANDARD.encode(payload))
    }
}

pub async fn solve_challenge(
    challenge: DeepSeekPowChallenge,
    timeout_ms: u64,
) -> Result<(u64, String), String> {
    challenge.validate()?;
    let _slot = timeout(
        Duration::from_millis(timeout_ms.max(1)),
        POW_SLOTS.acquire(),
    )
    .await
    .map_err(|_| "DeepSeek PoW worker capacity wait timed out".to_string())?
    .map_err(|_| "DeepSeek PoW worker semaphore closed".to_string())?;

    let worker_challenge = challenge.clone();
    let handle = task::spawn_blocking(move || solve_challenge_sync(&worker_challenge));
    let answer = timeout(Duration::from_millis(timeout_ms.max(1)), handle)
        .await
        .map_err(|_| format!("DeepSeek PoW computation exceeded {timeout_ms}ms"))?
        .map_err(|error| format!("DeepSeek PoW worker failed: {error}"))??;
    let header = challenge.response_header(answer)?;
    Ok((answer, header))
}

fn solve_challenge_sync(challenge: &DeepSeekPowChallenge) -> Result<u64, String> {
    let target = challenge.validate()?;
    let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);
    for nonce in 0..challenge.difficulty {
        let mut candidate = String::with_capacity(prefix.len() + 20);
        candidate.push_str(&prefix);
        candidate.push_str(&nonce.to_string());
        if deepseek_hash_v1(candidate.as_bytes()) == target {
            return Ok(nonce);
        }
    }
    Err("DeepSeek PoW challenge had no answer within its declared difficulty".into())
}

fn decode_digest(value: &str) -> Result<[u8; OUTPUT_BYTES], String> {
    if value.len() != OUTPUT_BYTES * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("DeepSeek PoW challenge must be a 64-character hex digest".into());
    }
    let mut out = [0_u8; OUTPUT_BYTES];
    for (index, byte) in out.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| "DeepSeek PoW challenge contains invalid hex".to_string())?;
    }
    Ok(out)
}

fn deepseek_hash_v1(input: &[u8]) -> [u8; OUTPUT_BYTES] {
    let mut state = [0_u64; 25];
    let mut offset = 0;
    while offset + RATE_BYTES <= input.len() {
        absorb_block(&mut state, &input[offset..offset + RATE_BYTES]);
        offset += RATE_BYTES;
    }

    let mut final_block = [0_u8; RATE_BYTES];
    let remaining = &input[offset..];
    final_block[..remaining.len()].copy_from_slice(remaining);
    final_block[remaining.len()] ^= DOMAIN_SUFFIX;
    final_block[RATE_BYTES - 1] ^= 0x80;
    absorb_block(&mut state, &final_block);

    let mut output = [0_u8; OUTPUT_BYTES];
    for (index, byte) in output.iter_mut().enumerate() {
        let lane = index / 8;
        let shift = (index % 8) * 8;
        *byte = ((state[lane] >> shift) & 0xff) as u8;
    }
    output
}

fn absorb_block(state: &mut [u64; 25], block: &[u8]) {
    debug_assert_eq!(block.len(), RATE_BYTES);
    for (index, byte) in block.iter().enumerate() {
        let lane = index / 8;
        state[lane] ^= (*byte as u64) << ((index % 8) * 8);
    }
    keccak_p1600_23(state);
}

fn keccak_p1600_23(state: &mut [u64; 25]) {
    let mut column = [0_u64; 5];
    let mut mix = [0_u64; 5];
    let mut rho_pi = [0_u64; 25];

    for round in (ROUND_CONSTANTS.len() - ROUNDS)..ROUND_CONSTANTS.len() {
        for x in 0..5 {
            column[x] =
                state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
        }
        for x in 0..5 {
            mix[x] = column[(x + 4) % 5] ^ column[(x + 1) % 5].rotate_left(1);
        }
        for y in 0..5 {
            for x in 0..5 {
                state[x + 5 * y] ^= mix[x];
            }
        }

        for y in 0..5 {
            for x in 0..5 {
                let lane = x + 5 * y;
                let dest_x = y;
                let dest_y = (2 * x + 3 * y) % 5;
                rho_pi[dest_x + 5 * dest_y] =
                    state[lane].rotate_left(ROTATION_OFFSETS[lane]);
            }
        }

        for y in 0..5 {
            let row = 5 * y;
            for x in 0..5 {
                state[row + x] = rho_pi[row + x]
                    ^ ((!rho_pi[row + (x + 1) % 5]) & rho_pi[row + (x + 2) % 5]);
            }
        }
        state[0] ^= ROUND_CONSTANTS[round];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: [u8; OUTPUT_BYTES]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn deepseek_hash_v1_matches_known_clean_room_vectors() {
        let vectors = [
            (
                "1122334455667788_1778891543095_0",
                "311b26ae1e0fe7375e242958ce46db5552a6c67fea3f96880dcd846c63a74286",
            ),
            (
                "1122334455667788_1778891543095_1",
                "526f9103dfe22bcda9481b7f304a157b8edb18c5bb96a2061eefec1fbd0db706",
            ),
            (
                "vector_42_3",
                "2ffed26ea9e1d6f4bbe49a266d98fe04ad9a5ad4c6d765862de8d18c513c3815",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(hex(deepseek_hash_v1(input.as_bytes())), expected, "{input}");
        }
    }

    #[test]
    fn solver_finds_known_nonzero_nonce() {
        let challenge = DeepSeekPowChallenge {
            algorithm: "DeepSeekHashV1".into(),
            challenge:
                "2ffed26ea9e1d6f4bbe49a266d98fe04ad9a5ad4c6d765862de8d18c513c3815"
                    .into(),
            salt: "vector".into(),
            signature: "test-signature".into(),
            difficulty: 4,
            expire_at: 42,
            target_path: COMPLETION_PATH.into(),
        };
        assert_eq!(solve_challenge_sync(&challenge).unwrap(), 3);
    }

    #[test]
    fn solver_rejects_unbounded_or_unknown_challenges() {
        let mut challenge = DeepSeekPowChallenge {
            algorithm: "UnknownHash".into(),
            challenge: "f".repeat(64),
            salt: "salt".into(),
            signature: "test-signature".into(),
            difficulty: 1,
            expire_at: 1,
            target_path: COMPLETION_PATH.into(),
        };
        assert!(challenge.validate().unwrap_err().contains("unsupported"));
        challenge.algorithm = "DeepSeekHashV1".into();
        challenge.difficulty = MAX_DIFFICULTY + 1;
        assert!(challenge.validate().unwrap_err().contains("difficulty"));
    }
}
