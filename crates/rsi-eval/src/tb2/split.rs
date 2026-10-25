//! Algorithm tb2-split-v1, including the original string-seeded MT19937 offset.
use super::{Result, dataset::Task};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
pub const SHARD_CAP_BYTES: u64 = 12_000_000_000;
pub const HEAVY_BYTES: u64 = 4_000_000_000;
const ORDER: [&str; 3] = ["easy", "medium", "hard"];
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Split {
    #[serde(default = "seed")]
    pub seed: u64,
    #[serde(default = "algorithm")]
    pub algorithm: String,
    pub search: Vec<String>,
    pub held_out: Vec<String>,
    #[serde(default)]
    pub smoke: Vec<String>,
    #[serde(default)]
    pub calibration: Vec<String>,
}
fn seed() -> u64 {
    3135
}
fn algorithm() -> String {
    "tb2-split-v1".into()
}
impl Split {
    pub fn text(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shard {
    pub name: String,
    pub split: String,
    pub heavy: bool,
    pub bytes: u64,
    pub tasks: Vec<String>,
}
fn allocate(total: usize, counts: [usize; 3]) -> Result<[usize; 3]> {
    let n: usize = counts.iter().sum();
    if n == 0 {
        return Err("cannot allocate an empty split".into());
    }
    let quotas = counts.map(|c| total as f64 * c as f64 / n as f64);
    let mut alloc = quotas.map(|q| q.floor() as usize);
    let mut order = [0, 1, 2];
    order.sort_by(|&a, &b| (quotas[b] - alloc[b] as f64).total_cmp(&(quotas[a] - alloc[a] as f64)));
    for &i in order.iter().take(total - alloc.iter().sum::<usize>()) {
        alloc[i] += 1;
    }
    Ok(alloc)
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
/// CPython's version-2 string seed is the big-endian integer of text || sha512(text).
/// MT init_by_array consumes that integer in little-endian 32-bit words; random() uses
/// 27 + 26 bits of two tempered outputs. This preserves the published split exactly.
fn offset_random(text: &str) -> f64 {
    let mut bytes = text.as_bytes().to_vec();
    bytes.extend_from_slice(&Sha512::digest(text.as_bytes()));
    bytes.reverse();
    let key: Vec<u32> = bytes
        .chunks(4)
        .map(|chunk| {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            u32::from_le_bytes(word)
        })
        .collect();
    let mut mt = [0u32; 624];
    mt[0] = 19650218;
    for i in 1..624 {
        mt[i] = 1812433253u32
            .wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30))
            .wrapping_add(i as u32);
    }
    let (mut i, mut j) = (1, 0);
    for _ in 0..624.max(key.len()) {
        mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1664525))
            .wrapping_add(key[j])
            .wrapping_add(j as u32);
        i += 1;
        j += 1;
        if i >= 624 {
            mt[0] = mt[623];
            i = 1;
        }
        if j >= key.len() {
            j = 0;
        }
    }
    for _ in 0..623 {
        mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1566083941))
            .wrapping_sub(i as u32);
        i += 1;
        if i >= 624 {
            mt[0] = mt[623];
            i = 1;
        }
    }
    mt[0] = 0x80000000;
    for i in 0..624 {
        let y = (mt[i] & 0x80000000) | (mt[(i + 1) % 624] & 0x7fffffff);
        mt[i] = mt[(i + 397) % 624] ^ (y >> 1) ^ if y & 1 != 0 { 0x9908b0df } else { 0 };
    }
    let temper = |mut y: u32| {
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^= y >> 18;
        y
    };
    ((temper(mt[0]) >> 5) as f64 * 67108864.0 + (temper(mt[1]) >> 6) as f64) / 9007199254740992.0
}
pub fn make_split(rows: &[Task], seed: u64) -> Result<Split> {
    if rows.is_empty() {
        return Err("dataset has no tasks".into());
    }
    if rows.iter().any(|r| !ORDER.contains(&r.difficulty.as_str())) {
        return Err("unknown task difficulty".into());
    }
    let strata = ORDER.map(|d| {
        rows.iter()
            .filter(|r| r.difficulty == d)
            .collect::<Vec<_>>()
    });
    let alloc = allocate(30, strata.each_ref().map(Vec::len))?;
    let mut search = Vec::new();
    for (idx, mut list) in strata.into_iter().enumerate() {
        list.sort_by_cached_key(|r| (r.category.clone(), hash(&format!("{seed}:{}", r.name))));
        let k = alloc[idx];
        let m = list.len();
        if k == 0 {
            continue;
        }
        let off = offset_random(&format!("{seed}:{}", ORDER[idx])) * m as f64 / k as f64;
        let mut indices = std::collections::BTreeSet::new();
        for i in 0..k {
            indices.insert((off + i as f64 * m as f64 / k as f64).floor() as usize);
        }
        for i in indices {
            search.push(list.get(i).ok_or("split index out of range")?.name.clone());
        }
    }
    search.sort();
    let mut held_out: Vec<_> = rows
        .iter()
        .filter(|r| !search.contains(&r.name))
        .map(|r| r.name.clone())
        .collect();
    held_out.sort();
    let mut cheap: Vec<_> = rows
        .iter()
        .filter(|r| search.contains(&r.name) && r.agent_timeout_sec <= 900.0)
        .collect();
    cheap.sort_by(|a, b| (a.image_bytes, &a.name).cmp(&(b.image_bytes, &b.name)));
    let mut smoke: Vec<_> = cheap.into_iter().take(5).map(|r| r.name.clone()).collect();
    smoke.sort();
    let rest = ORDER.map(|d| {
        rows.iter()
            .filter(|r| r.difficulty == d && search.contains(&r.name) && !smoke.contains(&r.name))
            .collect::<Vec<_>>()
    });
    let cal_alloc = allocate(10, rest.each_ref().map(Vec::len))?;
    let mut calibration = Vec::new();
    for (idx, mut list) in rest.into_iter().enumerate() {
        list.sort_by_cached_key(|r| hash(&format!("{seed}:cal:{}", r.name)));
        calibration.extend(
            list.into_iter()
                .take(cal_alloc[idx])
                .map(|r| r.name.clone()),
        );
    }
    calibration.sort();
    Ok(Split {
        seed,
        algorithm: algorithm(),
        search,
        held_out,
        smoke,
        calibration,
    })
}
pub fn plan_shards(rows: &[Task], split: &Split) -> Vec<Shard> {
    let mut shards = Vec::new();
    for (part, names) in [("search", &split.search), ("held_out", &split.held_out)] {
        let mut members: Vec<_> = rows.iter().filter(|r| names.contains(&r.name)).collect();
        members.sort_by(|a, b| b.image_bytes.cmp(&a.image_bytes).then(a.name.cmp(&b.name)));
        let mut bins: Vec<(bool, u64, Vec<String>)> = Vec::new();
        for r in members.iter().filter(|r| r.image_bytes < HEAVY_BYTES) {
            if let Some(b) = bins
                .iter_mut()
                .find(|b| b.1 + r.image_bytes <= SHARD_CAP_BYTES)
            {
                b.1 += r.image_bytes;
                b.2.push(r.name.clone());
            } else {
                bins.push((false, r.image_bytes, vec![r.name.clone()]));
            }
        }
        bins.extend(
            members
                .iter()
                .filter(|r| r.image_bytes >= HEAVY_BYTES)
                .map(|r| (true, r.image_bytes, vec![r.name.clone()])),
        );
        for (i, (heavy, bytes, mut tasks)) in bins.into_iter().enumerate() {
            tasks.sort();
            shards.push(Shard {
                name: format!("{}-{:02}", part.replace('_', "-"), i + 1),
                split: part.into(),
                heavy,
                bytes,
                tasks,
            });
        }
    }
    shards
}
